// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Exactly-once decision key for a tool approval request. */
public record ApprovalId(String value) {
    /** Validates the stable approval prefix so IDs cannot cross protocol domains or grow without bound. */
    public ApprovalId {
        IdChecks.require(value, "appr_");
    }

    @Override public String toString() { return value; }
}
