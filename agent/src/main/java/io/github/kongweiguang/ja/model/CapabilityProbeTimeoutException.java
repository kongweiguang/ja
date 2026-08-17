// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

/** Internal stable signal for transports that observe the shared absolute deadline. */
final class CapabilityProbeTimeoutException extends RuntimeException {
    /** Suppresses provider text and stack capture because the timeout is already classified by code. */
    CapabilityProbeTimeoutException() {
        super(null, null, false, false);
    }
}
