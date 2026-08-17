// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

/** Probe outcomes are explicit so a failed probe is not rendered as a successful capability claim. */
public enum CapabilityProbeStatus {
    SUCCESS,
    FAILED,
    TIMEOUT,
    CANCELLED
}
