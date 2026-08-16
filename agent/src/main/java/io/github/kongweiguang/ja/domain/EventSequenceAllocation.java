// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Result of one atomic durable event-id/sequence allocation. */
public record EventSequenceAllocation(long seq, boolean duplicate) {
    /** Rejects values that cannot be represented safely by the JSON wire contract. */
    public EventSequenceAllocation {
        if (seq < 1 || seq > 9_007_199_254_740_991L) {
            throw new IllegalArgumentException("seq out of JSON safe integer range");
        }
    }
}
