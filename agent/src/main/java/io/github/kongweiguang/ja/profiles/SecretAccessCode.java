// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.profiles;

/** Stable secret-boundary categories safe for UI and telemetry without provider exception text. */
public enum SecretAccessCode {
    EMPTY,
    CLOSED,
    RESOLVER_FAILED,
    CALLBACK_FAILED
}
