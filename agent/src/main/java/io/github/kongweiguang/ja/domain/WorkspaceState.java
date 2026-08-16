// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.domain;

import io.github.kongweiguang.ja.protocol.UnicodeChecks;

import java.util.Objects;

/** Immutable workspace registration metadata; filesystem mutation belongs to a tool port. */
public record WorkspaceState(WorkspaceId workspaceId, String displayName, String rootPath,
                             Trust trust, boolean archived) {
    /** Validates user-visible metadata without granting filesystem access to this value object. */
    public WorkspaceState {
        Objects.requireNonNull(workspaceId, "workspaceId");
        if (displayName == null || displayName.length() > 256 || displayName.contains("\n")) {
            throw new IllegalArgumentException("invalid displayName");
        }
        UnicodeChecks.wellFormed(displayName, "workspace displayName");
        if (rootPath == null || rootPath.isBlank() || rootPath.length() > 4096) {
            throw new IllegalArgumentException("invalid rootPath");
        }
        UnicodeChecks.wellFormed(rootPath, "workspace rootPath");
        Objects.requireNonNull(trust, "trust");
    }

    /** Omits display names and absolute roots from logs while preserving state identity. */
    @Override
    public String toString() {
        return "WorkspaceState[workspaceId=" + workspaceId + ", trust=" + trust
                + ", archived=" + archived + "]";
    }

    /** Trust is user intent only and does not grant technical sandbox access. */
    public enum Trust { UNTRUSTED, TRUSTED }
}
