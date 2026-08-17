// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

/** Stable factory failure when a model lacks a successful capability decision. */
public final class CapabilityProbeException extends IllegalStateException {
    /** Exposes only the probe status/code, never transport exception text or endpoint details. */
    public CapabilityProbeException(CapabilityProbeResult result) {
        super("model capability probe is not successful: " + result.status() + "/" + result.failureCode());
    }
}
