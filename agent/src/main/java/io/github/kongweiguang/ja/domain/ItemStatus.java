// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Frozen timeline item lifecycle values from ja-rpc/v1. */
public enum ItemStatus {
    STARTED,
    IN_PROGRESS,
    COMPLETED,
    FAILED,
    CANCELLED;

    /** Indicates that no later delta/update may mutate the item. */
    public boolean terminal() {
        return this == COMPLETED || this == FAILED || this == CANCELLED;
    }
}
