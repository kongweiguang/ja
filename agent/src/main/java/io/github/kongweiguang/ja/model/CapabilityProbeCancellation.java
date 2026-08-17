// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

import java.util.concurrent.atomic.AtomicBoolean;

/** Cooperative cancellation token used to stop a probe without retaining a future or thread. */
public final class CapabilityProbeCancellation {
    private final AtomicBoolean cancelled = new AtomicBoolean();

    /** Requests cancellation once; repeated calls are intentionally idempotent. */
    public void cancel() {
        cancelled.set(true);
    }

    /** Lets the bounded probe loop observe cancellation between transport operations. */
    public boolean isCancelled() {
        return cancelled.get();
    }
}
