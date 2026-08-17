// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

import io.github.kongweiguang.ja.profiles.CapabilitySet;
import java.util.Objects;

/** Cached capability decision tied to the exact secret-free profile fingerprint. */
public record CapabilityProbeResult(
        String profileRevision,
        String transportRevision,
        CapabilityProbeStatus status,
        CapabilitySet capabilities,
        CapabilityProbeFailureCode failureCode,
        String failureSummary) {
    /** Requires only stable bounded diagnostics so provider errors cannot leak URLs or credentials. */
    public CapabilityProbeResult {
        profileRevision = requireRevision(profileRevision, "profileRevision");
        transportRevision = requireRevision(transportRevision, "transportRevision");
        Objects.requireNonNull(status, "status");
        Objects.requireNonNull(capabilities, "capabilities");
        Objects.requireNonNull(failureCode, "failureCode");
        if (failureSummary != null && failureSummary.length() > 128) {
            throw new IllegalArgumentException("failureSummary is too long");
        }
        if (status == CapabilityProbeStatus.SUCCESS
                && (failureCode != CapabilityProbeFailureCode.NONE || failureSummary != null)) {
            throw new IllegalArgumentException("successful probe cannot have a failure code");
        }
        if (status != CapabilityProbeStatus.SUCCESS
                && (failureCode == CapabilityProbeFailureCode.NONE || failureSummary == null)) {
            throw new IllegalArgumentException("failed probe requires stable failure diagnostics");
        }
        if (status != CapabilityProbeStatus.SUCCESS && !capabilities.supported().isEmpty()) {
            throw new IllegalArgumentException("failed probe must have empty capabilities");
        }
        if (status != CapabilityProbeStatus.SUCCESS
                && !summary(failureCode).equals(failureSummary)) {
            throw new IllegalArgumentException("failure summary must use a stable redacted value");
        }
        if (!isCodeAllowed(status, failureCode)) {
            throw new IllegalArgumentException("probe status and failure code do not match");
        }
        if (status == CapabilityProbeStatus.TIMEOUT && failureCode != CapabilityProbeFailureCode.TIMEOUT) {
            throw new IllegalArgumentException("timeout result requires TIMEOUT code");
        }
        if (status == CapabilityProbeStatus.CANCELLED && failureCode != CapabilityProbeFailureCode.CANCELLED
                && failureCode != CapabilityProbeFailureCode.CLOSED) {
            throw new IllegalArgumentException("cancelled result requires CANCELLED or CLOSED code");
        }
    }

    /** Returns a successful result after a transport has verified its advertised features. */
    public static CapabilityProbeResult success(String profileRevision, String transportRevision,
                                                CapabilitySet capabilities) {
        return new CapabilityProbeResult(profileRevision, transportRevision, CapabilityProbeStatus.SUCCESS,
                capabilities, CapabilityProbeFailureCode.NONE, null);
    }

    /** Returns a fail-closed result with an intentionally fixed summary for the selected code. */
    public static CapabilityProbeResult failure(String profileRevision, String transportRevision,
                                                CapabilityProbeStatus status,
                                                CapabilityProbeFailureCode code) {
        if (status == CapabilityProbeStatus.SUCCESS) {
            throw new IllegalArgumentException("failure status must not be SUCCESS");
        }
        String safe = summary(code);
        return new CapabilityProbeResult(profileRevision, transportRevision, status,
                CapabilitySet.empty(), code, safe);
    }

    /** Compatibility accessor exposes only a stable code, never an exception or response message. */
    public String failureReason() {
        return failureCode == CapabilityProbeFailureCode.NONE ? null : failureCode.name();
    }

    /** Keeps revision fields bounded and registration-shaped so diagnostics cannot carry endpoint text. */
    private static String requireRevision(String value, String name) {
        Objects.requireNonNull(value, name);
        if (value.isEmpty() || value.length() > 64) {
            throw new IllegalArgumentException(name + " must be a bounded registration revision");
        }
        for (int index = 0; index < value.length(); index++) {
            char current = value.charAt(index);
            boolean asciiLetter = (current >= 'A' && current <= 'Z') || (current >= 'a' && current <= 'z');
            boolean asciiDigit = current >= '0' && current <= '9';
            if (!(asciiLetter || asciiDigit || current == '-' || current == '_' || current == '.'
                    || current == ':' || current == '~')) {
                throw new IllegalArgumentException(name + " must be a bounded registration revision");
            }
        }
        return value;
    }

    /** Returns the fixed UI-safe diagnostic associated with one failure code. */
    private static String summary(CapabilityProbeFailureCode code) {
        return switch (code) {
            case FAILED -> "provider capability probe failed";
            case TIMEOUT -> "provider capability probe timed out";
            case CANCELLED -> "provider capability probe cancelled";
            case CLOSED -> "capability probe cache is closed";
            case OVERLOADED -> "capability probe executor is saturated";
            case INVALID_OUTPUT -> "provider returned invalid capability output";
            case CACHE_FULL -> "capability probe cache is full";
            case PERMANENT_FAULT -> "capability probe transport is non-cooperative";
            case NONE -> throw new IllegalArgumentException("failure code is required");
        };
    }

    /** Prevents a caller from relabelling timeout or cancellation as an unrelated provider error. */
    private static boolean isCodeAllowed(CapabilityProbeStatus status, CapabilityProbeFailureCode code) {
        return switch (status) {
            case SUCCESS -> code == CapabilityProbeFailureCode.NONE;
            case FAILED -> code == CapabilityProbeFailureCode.FAILED
                    || code == CapabilityProbeFailureCode.CLOSED
                    || code == CapabilityProbeFailureCode.OVERLOADED
                    || code == CapabilityProbeFailureCode.INVALID_OUTPUT
                    || code == CapabilityProbeFailureCode.CACHE_FULL
                    || code == CapabilityProbeFailureCode.PERMANENT_FAULT;
            case TIMEOUT -> code == CapabilityProbeFailureCode.TIMEOUT;
            case CANCELLED -> code == CapabilityProbeFailureCode.CANCELLED
                    || code == CapabilityProbeFailureCode.CLOSED;
        };
    }
}
