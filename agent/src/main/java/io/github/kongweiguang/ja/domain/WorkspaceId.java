// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

/** Identity of a registered workspace, independent of its filesystem path. */
public record WorkspaceId(String value) {
    /** Validates the workspace prefix because lease ownership must be durable and canonical. */
    public WorkspaceId {
        IdChecks.require(value, "ws_");
    }

    @Override public String toString() { return value; }
}
