// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.model;

/** Stable, non-provider-specific failure codes safe to expose to UI and telemetry. */
public enum CapabilityProbeFailureCode {
    NONE,
    FAILED,
    TIMEOUT,
    CANCELLED,
    CLOSED,
    OVERLOADED,
    INVALID_OUTPUT,
    CACHE_FULL,
    PERMANENT_FAULT
}
