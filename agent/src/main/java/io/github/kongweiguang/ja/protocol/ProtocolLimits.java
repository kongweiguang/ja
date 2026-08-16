// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.protocol;

/**
 * Negotiated protocol limits. Validation happens at construction so every
 * transport and use case shares the same fail-closed bounds.
 */
public record ProtocolLimits(
        int maxFrameBytes,
        int maxInboundQueueFrames,
        int maxOutboundQueueFrames,
        int maxInFlightRequests,
        int maxPendingRequests,
        int maxItemDeltaBytes,
        int maxInlineToolOutputBytes,
        int maxArtifactBytes,
        int maxLogBytes,
        int defaultRequestDeadlineMs,
        int defaultApprovalDeadlineMs) {

    public static final int MIN_FRAME_BYTES = 1_024;
    public static final int MAX_FRAME_BYTES = 16 * 1024 * 1024;
    /** Smallest negotiated item delta accepted by the v1 schema. */
    public static final int MIN_ITEM_DELTA_BYTES = 256;
    /** Default streaming chunk budget keeps ordinary model events small. */
    public static final int DEFAULT_ITEM_DELTA_BYTES = 65_536;
    /** Schema-level negotiated ceiling; implementations may advertise a lower value. */
    public static final int ABSOLUTE_MAX_ITEM_DELTA_BYTES = 1_048_576;
    /** Compatibility alias for code that means the schema-level maximum. */
    public static final int MAX_ITEM_DELTA_BYTES = ABSOLUTE_MAX_ITEM_DELTA_BYTES;

    /** Validates every session budget before it can influence frame or item allocation. */
    public ProtocolLimits {
        range("maxFrameBytes", maxFrameBytes, MIN_FRAME_BYTES, MAX_FRAME_BYTES);
        range("maxInboundQueueFrames", maxInboundQueueFrames, 1, 10_000);
        range("maxOutboundQueueFrames", maxOutboundQueueFrames, 1, 10_000);
        range("maxInFlightRequests", maxInFlightRequests, 1, 1_024);
        range("maxPendingRequests", maxPendingRequests, 1, 1_024);
        range("maxItemDeltaBytes", maxItemDeltaBytes, MIN_ITEM_DELTA_BYTES, ABSOLUTE_MAX_ITEM_DELTA_BYTES);
        range("maxInlineToolOutputBytes", maxInlineToolOutputBytes, 1_024, MAX_FRAME_BYTES);
        range("maxArtifactBytes", maxArtifactBytes, 1_048_576, 1_073_741_824);
        range("maxLogBytes", maxLogBytes, 4_096, 67_108_864);
        range("defaultRequestDeadlineMs", defaultRequestDeadlineMs, 1_000, 3_600_000);
        range("defaultApprovalDeadlineMs", defaultApprovalDeadlineMs, 1_000, 3_600_000);
    }

    /** Returns the frozen v1 baseline used before negotiation. */
    public static ProtocolLimits defaults() {
        return new ProtocolLimits(4 * 1024 * 1024, 256, 1_024, 64, 64,
                DEFAULT_ITEM_DELTA_BYTES, 1_048_576, 268_435_456, 1_048_576, 120_000, 300_000);
    }

    /** Applies one shared inclusive range so every negotiated field fails closed identically. */
    private static void range(String name, int value, int min, int max) {
        if (value < min || value > max) {
            throw new IllegalArgumentException(name + " is outside protocol bounds");
        }
    }
}
