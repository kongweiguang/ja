// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Exactly-once approval outcomes. */
public enum ApprovalDecision {
    ALLOW_ONCE,
    ALLOW_SESSION,
    DENY,
    EXPIRED,
    DISCONNECTED
}
